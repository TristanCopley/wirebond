//! In steady state the engine allocates nothing: a datagram from a program through to the lane
//! packets it sends (Event::App to Out::send), a lane packet through to the datagrams it delivers
//! (Event::Packet to Out::deliver), and the timers between them (Event::Tick), in stable and in
//! bandwidth (§4.2: "never allocates on the send path in steady state"). The Go reference allocates
//! nothing for any of them either.
//!
//! A counting global allocator counts the allocations made on a thread while the harness has
//! armed common::meter, which it does only inside Session::handle, and not inside the harness's
//! own Out, which copies what it records.

mod common;

use common::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::net::SocketAddr;
use wirebond_core::{Event, FlowId, MODE_BANDWIDTH, MODE_PERFORMANCE, Mono, Msg, Out, SEC, SockId};

/// The system allocator, counting for common::meter.
struct Counting;

// SAFETY: every method hands its arguments to the system allocator unchanged and returns what it
// returns, so Counting keeps all of System's guarantees. Counting itself only reads and writes
// const-initialized thread-local Cells, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        meter::note();
        // SAFETY: the caller upholds `alloc`'s contract, which is System's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        meter::note();
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        meter::note();
        // SAFETY: the caller upholds `realloc`'s contract; `ptr` came from System through us.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract; `ptr` came from System through us.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The meter counts an allocation only while armed and not quiet, and the harness's recording
/// Out is quiet: without this the zeros below would prove nothing.
#[test]
fn the_meter_counts_the_engine_s_allocations_only() {
    let before = meter::allocs();
    meter::arm(true);
    let counted = black_box(Vec::<u8>::with_capacity(64));
    let quiet = meter::quiet(|| black_box(Vec::<u8>::with_capacity(64)));
    let mut rec = Recorder::default();
    rec.send(1, C_MESH, &[0; 100]);
    rec.deliver(&[0; 10]);
    rec.control(Msg::default());
    meter::arm(false);
    let unarmed = black_box(Vec::<u8>::with_capacity(64));
    assert_eq!(meter::allocs() - before, 1, "armed: one allocation; quiet, unarmed and the recorder's: none");
    assert_eq!(rec.calls.len(), 3);
    drop((counted, quiet, unarmed));
}

/// Moonlight at 10 Mbps over the hotel, in `mode`: 3 s to warm up, then 2 s measured.
fn steady(mode: &'static str) {
    let mut p = Pair::hotel();
    p.keep_wire = false;
    p.set_mode(mode);
    let (mut video, mut audio, mut input) = moonlight(10.0);
    p.play(3 * SEC, &mut [&mut video, &mut audio, &mut input]);
    let st = p.snap(Side::Server);
    let carriers = |st: &wirebond_core::SessionStatus| -> Vec<u16> {
        st.lanes.iter().filter(|l| l.out.role == "spread").map(|l| l.id).collect()
    };
    let (lanes, rewinds) = (carriers(&st), st.rewinds);
    assert_eq!(st.mode, mode, "the server's mode; {}", describe(&st));
    assert!(
        !lanes.is_empty(),
        "{mode}: no lane carries bulk after 3 s, so there is no steady state; {}",
        describe(&st)
    );
    assert!(p.c.got(VIDEO).len() > 1000, "{mode}: only {} video datagrams in 3 s", p.c.got(VIDEO).len());

    let (sent, got) = (p.s.count(VIDEO), p.c.got(VIDEO).len());
    p.tally = Some(BTreeMap::new());
    p.play(2 * SEC, &mut [&mut video, &mut audio, &mut input]);
    let tally = p.tally.take().expect("set");

    let st = p.snap(Side::Server);
    assert_eq!(
        (carriers(&st), st.rewinds),
        (lanes, rewinds),
        "{mode}: bulk moved while measuring, so this was no steady state; {}",
        describe(&st)
    );
    let (sent, got) = (p.s.count(VIDEO) - sent, p.c.got(VIDEO).len() - got);
    assert!(got as f64 >= 0.95 * f64::from(sent), "{mode}: {got} of {sent} video datagrams delivered while measuring");

    let at = |side, kind| tally.get(&(side, kind)).copied().unwrap_or_default();
    let mut report = String::new();
    for ((side, kind), t) in &tally {
        report += &format!(
            "\n  {side:?} {kind}: {} events, {} allocations, {} sends, {} deliveries",
            t.events, t.allocs, t.sends, t.delivers
        );
    }
    assert!(
        at(Side::Server, "app").sends > 0 && at(Side::Client, "packet").delivers > 1000,
        "{mode}: the measured events neither sent nor delivered:{report}"
    );
    for side in [Side::Client, Side::Server] {
        for kind in ["app", "packet"] {
            assert_eq!(
                at(side, kind).allocs,
                0,
                "{mode}: the {side:?}'s {kind} events allocated in steady state (§4.2):{report}"
            );
        }
    }
    for side in [Side::Client, Side::Server] {
        assert_eq!(at(side, "tick").allocs, 0, "{mode}: the {side:?}'s timers allocated in steady state:{report}");
    }
}

#[test]
fn stable_allocates_nothing_in_steady_state() {
    steady(MODE_PERFORMANCE);
}

#[test]
fn combine_allocates_nothing_in_steady_state() {
    steady(MODE_BANDWIDTH);
}

/// An Out that only counts, so that it allocates nothing itself.
#[derive(Default)]
struct Count {
    sends: u64,
}

impl Out for Count {
    fn send(&mut self, _: SockId, _: SocketAddr, _: &[u8]) {
        self.sends += 1;
    }

    fn deliver(&mut self, _: &[u8]) {}

    fn control(&mut self, _: Msg) {}
}

/// Go's TestSendAllocs: a keyed client whose peer never answers sends a 1,408-byte bulk datagram
/// and a 200-byte small one every 100 µs, with the timers it asks for; after 2 s, 5,000 more such
/// steps allocate nothing. One run of 5,000 steps, so that even one allocation in the batch shows.
///
/// In 2.0 each is one inner packet of its own flow (§10.3, §13), built once: a new flow allocates
/// its table entry in the first steps, and the steady state allocates nothing.
#[test]
fn the_send_path_allocates_nothing_while_the_peer_is_silent() {
    let mut k = Keyed::new(11, 12);
    let (bulk, small) = (inner(FlowId(1), &[0; 1408]), inner(FlowId(2), &[0; 200]));
    let mut out = Count::default();
    let mut now: Mono = 0;
    let mut step = |k: &mut Keyed, out: &mut Count| {
        now += 100_000;
        for _ in 0..1000 {
            if k.c.next() > now {
                break;
            }
            k.c.handle(now, Event::Tick, &mut k.c_rids, out);
        }
        assert!(k.c.next() > now, "no progress at {}: next() keeps asking for a tick at or before now", ms(now));
        k.c.handle(now, Event::Send { packet: &bulk }, &mut k.c_rids, out);
        k.c.handle(now, Event::Send { packet: &small }, &mut k.c_rids, out);
    };
    for _ in 0..20_000 {
        step(&mut k, &mut out);
    }
    let (before, sends) = (meter::allocs(), out.sends);
    meter::arm(true);
    for _ in 0..5_000 {
        step(&mut k, &mut out);
    }
    meter::arm(false);
    assert!(out.sends > sends, "nothing was sent in the measured steps");
    assert_eq!(meter::allocs() - before, 0, "the send path allocated in 5,000 steps ({} sends)", out.sends - sends);
}
