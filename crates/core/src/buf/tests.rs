//! Tests of buf.rs. Owner: C1. No Go tests: add the pool's BOND_BYTES cap (`get` returns None
//! at MAX_BUFS), reuse after `put`, and `copy`.

use crate::pair::*;

use std::collections::BTreeSet;

use crate::buf::{MAX_BUFS, Pkt, Pool, Ring};
use crate::{BOND_BYTES, MAX_PACKET};

/// The pool starts with 16 buffers, doubles when empty, and stops at MAX_BUFS, BOND_BYTES in
/// all (§7.6 rule 5); every buffer it gives is distinct.
#[test]
fn grows_to_the_cap() {
    let mut p = Pool::default();
    assert_eq!((p.len(), p.held()), (0, 0));
    let mut got = BTreeSet::new();
    let mut sizes = Vec::new();
    while let Some(i) = p.get() {
        assert!((i as usize) < p.len() && got.insert(i), "buffer {i} given twice");
        if sizes.last() != Some(&p.len()) {
            sizes.push(p.len());
        }
        assert_eq!(p.held(), got.len() * MAX_PACKET);
    }
    let mut want: Vec<_> = (4..14).map(|s| 1 << s).collect(); // 16, 32, … 8192
    want.push(MAX_BUFS);
    assert_eq!(sizes, want);
    assert_eq!((got.len(), p.len(), MAX_BUFS), (MAX_BUFS, MAX_BUFS, BOND_BYTES / MAX_PACKET));
    assert!(p.held() <= BOND_BYTES && p.held() + MAX_PACKET > BOND_BYTES);
    assert_eq!(p.get(), None, "a full pool gives nothing");
    // A buffer put back is the next one given; the pool does not grow again.
    p.put(1234);
    assert_eq!(p.held(), (MAX_BUFS - 1) * MAX_PACKET);
    assert_eq!((p.get(), p.get(), p.len()), (Some(1234), None, MAX_BUFS));
}

/// Buffers are MAX_PACKET bytes, apart from each other, and `copy` moves a prefix between them.
#[test]
fn buffers_and_copy() {
    let mut p = Pool::default();
    let (a, b) = (p.get().unwrap(), p.get().unwrap());
    assert_ne!(a, b);
    assert!(p.buf(a).len() == MAX_PACKET && p.buf(b).len() == MAX_PACKET);
    for (j, x) in p.buf_mut(a).iter_mut().enumerate() {
        *x = j as u8;
    }
    p.buf_mut(b).fill(0xee);
    p.copy(a, b, 300);
    assert!(p.buf(b)[..300].iter().enumerate().all(|(j, &x)| x == j as u8));
    assert!(p.buf(b)[300..].iter().all(|&x| x == 0xee) && p.buf(a)[299] == 43);
    // Put back and taken again, a buffer keeps its index and its bytes (nothing is zeroed).
    p.put(a);
    assert_eq!(p.get(), Some(a));
    assert_eq!(p.buf(a)[7], 7);
}

/// Once warm, taking and putting back buffers allocates nothing, and nor does putting back every
/// buffer the pool ever gave (§4.2). A warm Ring allocates nothing either.
#[test]
fn warm_allocates_nothing() {
    let mut p = Pool::default();
    let taken: Vec<_> = (0..1000).map(|_| p.get().unwrap()).collect();
    let before = allocs();
    for &i in &taken {
        p.put(i);
    }
    for round in 0..100 {
        let a: [_; 64] = std::array::from_fn(|_| p.get().unwrap());
        for i in a.into_iter().rev() {
            p.put(i);
        }
        assert_eq!(p.held(), 0, "round {round}");
    }
    assert_eq!(allocs() - before, 0, "allocations in a warm pool");
    let mut r = Ring::new();
    for c in 0..100 {
        r.push_back(Pkt { c, ..Pkt::default() });
    }
    let before = allocs();
    for c in 100..10_000 {
        let p = r.pop_front().unwrap();
        assert_eq!(p.c, c - 100);
        r.push_back(Pkt { c, at: 0, buf: Some(c as u32), n: 1 });
    }
    assert_eq!((allocs() - before, r.len(), r[0].c), (0, 100, 9_900));
}
