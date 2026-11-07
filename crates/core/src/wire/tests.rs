//! Tests of wire.rs. Owner: C1. Ports from core/wire_test.go: TestReplayWindow, TestProbeCodec
//! (the 175-byte three-echo probe; `token`/`chal` replace `observed`), FuzzProbe (as a seeded
//! loop over DetRand inputs: decode then encode reproduces the input, with bytes 51–52 of each
//! echo masked, since they are written 0 and ignored on read).

use crate::pair::*;

use std::collections::BTreeSet;

use crate::wire::{Echo, Probe, Window, probe_len};
use crate::{ECHO_LEN, MAX_ECHOES, MAX_PACKET, PROBE_LEN, REPLAY_WINDOW};

/// Words in the window.
const N: u64 = REPLAY_WINDOW / 64;

/// The window against a set model while 2–4 lanes deliver copies of every counter with different
/// skews and jitter (§17.1). Go: TestReplayWindow.
#[test]
fn replay_window() {
    for lanes in 2..=4u64 {
        let mut r = DetRand(lanes);
        let mut arrivals = Vec::new();
        for c in 0..40_000u64 {
            for l in 0..lanes {
                arrivals.push((c + l * 3000 + r.next() % 600, c));
            }
        }
        arrivals.sort_by_key(|a| a.0); // stable, as Go's SortStableFunc
        let mut w = Window::default();
        let (mut seen, mut top, mut any, mut accepted) = (BTreeSet::new(), 0, false, 0);
        for (_, c) in arrivals {
            let want = !seen.contains(&c) && (!any || c > top || top / 64 - c / 64 < N);
            assert_eq!(w.check(c), want, "{lanes} lanes: counter {c} (top {top})");
            if want {
                seen.insert(c);
                accepted += 1;
                if !any || c > top {
                    (top, any) = (c, true);
                }
            }
        }
        assert!(accepted >= 40_000, "{lanes} lanes: only {accepted} of 40000 counters accepted");
    }
}

/// The window's width is 8,129 to 8,192 counters, depending on where the top falls in its word
/// (note 3), and no counter, however large, overflows it.
#[test]
fn window_edges() {
    for (top, low) in [(64 * 300 + 63, 64 * (300 - N + 1)), (64 * 300, 64 * (300 - N + 1))] {
        let mut w = Window::default();
        assert!(w.check(top));
        assert!(!w.check(low - 1), "top {top}: {} is outside", low - 1);
        assert!(w.check(low), "top {top}: {low} is inside");
        assert!(w.check(top - 1) && !w.check(top) && !w.check(low));
        assert!(top - low + 1 >= 8129 && top - low < REPLAY_WINDOW);
    }
    // A jump of a whole window or more clears every word.
    let mut w = Window::default();
    for c in 0..REPLAY_WINDOW {
        assert!(w.check(c));
    }
    assert!(w.check(REPLAY_WINDOW * 5) && w.bits.iter().filter(|&&b| b != 0).count() == 1);
    assert!(w.check(REPLAY_WINDOW * 5 - 1) && !w.check(REPLAY_WINDOW * 4));
    // The ends of the counter space.
    let mut w = Window::default();
    assert!(w.check(u64::MAX) && !w.check(u64::MAX) && !w.check(0) && w.check(u64::MAX - 1));
    let mut w = Window::default();
    assert!(w.check(0) && !w.check(0) && w.check(u64::MAX) && !w.check(0));
}

/// §10.4: `seen` answers as `check` would that a counter is not new (recorded, or below the
/// window) without recording anything, so a later copy is found before it is opened.
#[test]
fn window_seen_reads_only() {
    let mut w = Window::default();
    for c in [0, 1, 5, 64, 200, 9_000, 9_001, 20_000] {
        let before = w.clone();
        let seen = w.seen(c);
        assert_eq!(w, before, "seen({c}) changed the window");
        assert_eq!(seen, !w.clone().check(c), "seen({c}) and check disagree");
        w.check(c);
        assert!(w.seen(c), "{c} recorded");
    }
    assert!(w.seen(20_000 - REPLAY_WINDOW), "below the window");
    assert!(!w.seen(20_001) && !w.seen(19_999), "not recorded");
}

/// The three echoes of TestProbeCodec, with challenges in place of Observed.
fn three() -> [Echo; 3] {
    [
        Echo {
            lane: 0x0102,
            flags: 1,
            seq: 6,
            held: 900,
            owd: 31000,
            pkts: 1 << 31,
            bytes: 5,
            last: (1 << 40) + 3,
            token: 0x0102_0304_0506_0708,
            chal: u64::MAX,
        },
        Echo { lane: 0, seq: 1, owd: -4, token: 1, ..Echo::default() },
        Echo { lane: 0x0201, ..Echo::default() },
    ]
}

/// Encode and decode round trip; the 175-byte three-echo probe (§8.2, §17.1). Go: TestProbeCodec.
#[test]
fn probe_codec() {
    let echoes = three();
    let p = Probe { lane: 0x0102, seq: 7, sent_at: -5, flags: 1, n: 3 };
    let mut b = vec![0xee; MAX_PACKET];
    let n = p.encode(&echoes, &mut b);
    assert_eq!(n, 175, "three-echo probe is {n} bytes, want 175 (§8.2)");
    assert_eq!(n, probe_len(3));
    assert_eq!(b[n], 0xee, "encode wrote past the probe");
    // Big-endian, fixed-width: the header, then each echo's fields at their offsets.
    assert_eq!(b[..16], [1, 2, 0, 0, 0, 7, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfb, 1, 3]);
    let e0 = &b[PROBE_LEN..PROBE_LEN + ECHO_LEN];
    assert_eq!((e0[..3].to_vec(), e0[19], e0[27..35].to_vec()), (vec![1, 2, 1], 0x80, vec![0, 0, 1, 0, 0, 0, 0, 3]));
    assert_eq!(
        (e0[35..43].to_vec(), e0[43..51].to_vec(), e0[51..].to_vec()),
        ((1..=8).collect(), vec![0xff; 8], vec![0, 0])
    );
    let mut q = Probe::default();
    let mut buf = [Echo::default(); MAX_ECHOES];
    assert!(q.decode(&b[..n], &mut buf), "decode failed");
    assert_eq!((q, buf[..3].to_vec()), (p, echoes.to_vec()));
    // Bytes 51–52 of an echo are ignored on read (a Go peer writes the observed port there).
    b[PROBE_LEN + 51] = 0x1f;
    b[PROBE_LEN + 2 * ECHO_LEN + 52] = 0x90;
    let mut q2 = Probe::default();
    assert!(q2.decode(&b[..n], &mut buf) && q2 == p && buf[..3] == echoes);
    // A short probe, a long one, a wrong count, or more echoes than the buffer: rejected, the
    // probe unchanged.
    let mut q3 = Probe { seq: 99, ..Probe::default() };
    assert!(!q3.decode(&b[..n - 1], &mut buf) && !q3.decode(&b[..n + 1], &mut buf) && q3.seq == 99);
    assert!(!q3.decode(&b[..n], &mut buf[..2]) && q3.seq == 99, "decode accepted more echoes than its buffer");
    b[15] = 2;
    assert!(!q3.decode(&b[..n], &mut buf) && q3.decode(&b[..probe_len(2)], &mut buf) && q3.n == 2);
    // No echoes; the most echoes a probe can carry.
    let mut b = vec![0; MAX_PACKET];
    let p = Probe { n: 0, ..p };
    assert_eq!(p.encode(&echoes, &mut b), PROBE_LEN);
    assert!(q.decode(&b[..PROBE_LEN], &mut buf) && q == p);
    let full = [echoes[0]; MAX_ECHOES];
    let p = Probe { n: MAX_ECHOES, ..p };
    let n = p.encode(&full, &mut b);
    assert!(n <= MAX_PACKET - crate::HEADER_LEN - crate::TAG_LEN && q.decode(&b[..n], &mut buf));
    assert_eq!((q, buf), (p, full));
}

/// `b` with bytes 51–52 of each echo zeroed: what decode then encode gives back.
fn masked(b: &[u8]) -> Vec<u8> {
    let mut m = b.to_vec();
    for o in m[PROBE_LEN..].as_chunks_mut::<ECHO_LEN>().0 {
        o[51..].fill(0);
    }
    m
}

/// Decode never panics; whatever it accepts, encode gives back, but for the ignored bytes; what it
/// rejects leaves the probe as it was. Go: FuzzProbe, here a seeded loop over random inputs, whole
/// probes of every size, and mutations of a valid one.
#[test]
fn fuzz_probe() {
    let mut r = DetRand(0x5eed);
    let echoes = three();
    let mut seed = vec![0; MAX_PACKET];
    let len = Probe { lane: 1, seq: 2, n: 1, ..Probe::default() }.encode(&echoes, &mut seed);
    seed.truncate(len);
    let (mut accepted, mut rejected) = (0, 0);
    for i in 0..20_000 {
        let mut b = match i % 4 {
            // Random bytes of any length.
            0 => {
                let mut b = vec![0; (r.next() % (MAX_PACKET as u64 + 1)) as usize];
                r.fill(&mut b);
                b
            }
            // A whole probe of random bytes with a count that fits it.
            1 => {
                let n = (r.next() % (MAX_ECHOES as u64 + 1)) as usize;
                let mut b = vec![0; probe_len(n)];
                r.fill(&mut b);
                b[15] = n as u8;
                b
            }
            // The seed probe with a few bytes changed, cut short or grown.
            _ => {
                let mut b = seed.clone();
                for _ in 0..1 + r.next() % 4 {
                    let j = (r.next() % b.len() as u64) as usize;
                    b[j] = r.next() as u8;
                }
                match r.next() % 8 {
                    0 => b.truncate((r.next() % b.len() as u64) as usize),
                    1 => b.extend_from_slice(&[0; ECHO_LEN]),
                    _ => {}
                }
                b
            }
        };
        let cap = (r.next() % (MAX_ECHOES as u64 + 1)) as usize;
        let mut buf = [Echo::default(); MAX_ECHOES];
        let mut p = Probe { seq: 0xdead, ..Probe::default() };
        if !p.decode(&b, &mut buf[..cap]) {
            assert_eq!(p, Probe { seq: 0xdead, ..Probe::default() }, "a rejected probe changed the decoder");
            rejected += 1;
            continue;
        }
        accepted += 1;
        assert!(p.n <= cap && b.len() == probe_len(p.n));
        let mut out = vec![0xaa; b.len()];
        assert_eq!(p.encode(&buf, &mut out), b.len());
        b = masked(&b);
        assert_eq!(out, b, "re-encoding differs");
    }
    assert!(accepted > 3_000 && rejected > 3_000, "{accepted} accepted, {rejected} rejected");
}
