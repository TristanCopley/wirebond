//! Tests of keys.rs. Owner: C1. Ports from core/wire_test.go: TestSealOpen, TestRatchet,
//! TestPeerNewKey, TestForgedCounter, FuzzPeer and FuzzOpen (both as seeded loops over DetRand
//! inputs). New: Challenger tokens are deterministic per key, differ per (lane, gen, seq), and
//! are never 0; RidTable alloc skips 0 and held ids, free only frees the owner's.
//!
//! The ports drive whole Sessions. The tests before them check the same properties on Sender and
//! Receiver alone, plus vectors from Go's crypto (interop), the counter floor (R2-09) and the
//! absence of allocation.

use crate::pair::*;

use std::collections::BTreeSet;

use crate::keys::{Challenger, EpochKey, Receiver, Sender, Slot};
use crate::wire::{Echo, Probe};
use crate::{
    BOND_BYTES, DATA_LEN, EPOCH_BITS, Event, FlowId, HEADER_LEN, MAX_FLOWS, MAX_PACKET, MODE_PERFORMANCE, MeshLane,
    Mono, Msg, OVERHEAD, RidTable, Role, SEC, SMALL_BIT, Session, TAG_LEN, TYPE_DATA, TYPE_PROBE,
};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

/// The key 00 01 … 1f.
fn k0() -> [u8; 32] {
    std::array::from_fn(|i| i as u8)
}

/// A sender with `k` at epoch 0 and a new receiver holding it.
fn keys(k: [u8; 32]) -> (Sender, Receiver) {
    let mut r = Receiver::default();
    assert!(!r.install_from(0, &k, 0), "a first key is not fresh");
    (Sender { key: EpochKey::new(0, k), ctr: 0, rid: 0x1234 }, r)
}

/// `pt` sealed by `s` as a `typ` packet, and whether `s` entered an epoch.
fn seal(s: &mut Sender, typ: u8, pt: &[u8]) -> (Vec<u8>, bool) {
    let mut b = vec![0; MAX_PACKET];
    b[HEADER_LEN..HEADER_LEN + pt.len()].copy_from_slice(pt);
    let (n, fresh) = s.seal(&mut b, typ, pt.len());
    b.truncate(n);
    (b, fresh)
}

/// `b` received as §7.5 orders it, on the keys alone: open, the session window, then advance on a
/// next-epoch packet. The plaintext and the slot that opened it.
fn recv_keys(r: &mut Receiver, b: &[u8]) -> Option<(Vec<u8>, Slot)> {
    let mut dst = vec![0; MAX_PACKET];
    let (n, slot) = r.open(&mut dst, b)?;
    if !r.win.check(u64::from_be_bytes(b[3..HEADER_LEN].try_into().unwrap())) {
        return None;
    }
    if slot == Slot::Next {
        r.advance();
    }
    Some((dst[..n].to_vec(), slot))
}

/// What a receiver holds, comparable.
fn state(r: &Receiver) -> RxState {
    let k = |e: &EpochKey| (e.e, e.k, e.is_set());
    RxState { keys: [k(&r.prev), k(&r.cur), k(&r.next)], win: r.win.clone(), floor: r.floor }
}

/// Byte-for-byte what Go's crypto/hmac, crypto/aes and crypto/cipher give for keys.go's ratchet
/// and seal, so a Go peer and a Rust peer open each other's packets.
#[test]
fn go_vectors() {
    let k1 = EpochKey::new(0, k0()).ratchet();
    assert_eq!((k1.e, k1.k.to_vec()), (1, hex("c137ef2e8a390c45a1021b0a9cee98876e59e8fb83758e36f1c798435b3405d5")));
    let k2 = k1.ratchet();
    assert_eq!((k2.e, k2.k.to_vec()), (2, hex("29d21c4a8d153048052630475accb23bc9e5dcc1dc5e1004a1d0b2c7f3ea3ca0")));
    let (mut s, mut r) = keys(k0());
    s.ctr = 5;
    let (p0, _) = seal(&mut s, TYPE_DATA | SMALL_BIT, b"audio");
    assert_eq!(p0, hex("811234000000000000000532ff37a1717ed020dd2698dfdcd762d1743de86428"));
    (s.ctr, s.rid) = (1 << EPOCH_BITS, 0xbeef);
    let (p1, fresh) = seal(&mut s, TYPE_PROBE, &[0xa5; 16]);
    assert!(fresh && s.key.e == 1 && s.key.k == k1.k);
    assert_eq!(p1, hex("02beef0000000001000000ae13dcdad6ca3872c58ed4f6ae21f8cc0ff7293d4f3c17df784e389eb631813b"));
    let mut s = Sender { key: EpochKey::new(0, k0()), ctr: 0, rid: 1 };
    let (p2, fresh) = seal(&mut s, TYPE_DATA, &[]);
    assert!(!fresh);
    assert_eq!(p2, hex("010001000000000000000083f8fdc533ec736bb359d91a4dbc4660"));
    assert_eq!(recv_keys(&mut r, &p0), Some((b"audio".to_vec(), Slot::Cur)));
    assert_eq!(recv_keys(&mut r, &p2), Some((vec![], Slot::Cur)));
    assert_eq!(recv_keys(&mut r, &p1), Some((vec![0xa5; 16], Slot::Next)));
    // The challenge PRF: HMAC-SHA256 over lane, gen and seq, big-endian, first 8 bytes.
    assert_eq!(Challenger::new(k0()).token(0x0102, 3, 7), 0xa1bdf42cebef38ab);
    assert_eq!(Challenger::new(k1.k).token(0, 0, 0), 0xd5c37bbbb8fa5477);
}

/// Seal and open on the keys alone: any non-authentic bytes are dropped with no state change and
/// `dst` untouched; the genuine packet opens once (§7.3, §7.5, §17.1).
#[test]
fn sender_receiver() {
    let (mut s, mut r) = keys(k0());
    let pt = [&[0xbb, 0x80, 0, 0, 0, 1][..], b"audio"].concat(); // flow (2) + seq (4), §10.3
    let (pkt, _) = seal(&mut s, TYPE_DATA | SMALL_BIT, &pt);
    assert_eq!(pkt.len(), OVERHEAD + 5);
    assert_eq!(pkt[..HEADER_LEN], [0x81, 0x12, 0x34, 0, 0, 0, 0, 0, 0, 0, 0]);
    let before = state(&r);
    let mut dst = vec![0x5a; MAX_PACKET];
    for i in 0..pkt.len() {
        for bit in [1, 0x80] {
            let mut b = pkt.clone();
            b[i] ^= bit;
            assert_eq!(r.open(&mut dst, &b), None, "byte {i} flipped");
        }
    }
    let n = pkt.len();
    let longer = [&pkt[..], &[0]].concat();
    for b in [&[][..], &pkt[..HEADER_LEN + TAG_LEN - 1], &pkt[..n - 1], &longer, &pkt[1..]] {
        assert_eq!(r.open(&mut dst, b), None, "{}-byte packet", b.len());
    }
    assert!(dst.iter().all(|&x| x == 0x5a) && state(&r) == before, "a dropped packet left a trace");
    assert_eq!(r.open(&mut dst[..DATA_LEN + 4], &pkt), None, "plaintext longer than dst");
    assert_eq!(recv_keys(&mut r, &pkt), Some((pt.clone(), Slot::Cur)));
    assert_eq!(recv_keys(&mut r, &pkt), None, "replay");
    // A receiver without a key opens nothing; the counter goes up by one per packet.
    assert_eq!(Receiver::default().open(&mut dst, &pkt), None);
    let (p2, _) = seal(&mut s, TYPE_PROBE, &[]);
    assert_eq!((s.ctr, p2[3..HEADER_LEN].to_vec()), (2, vec![0, 0, 0, 0, 0, 0, 0, 1]));
}

/// The ratchet across an epoch boundary: the sender ratchets once and reports it; the receiver
/// follows on the first packet of the next epoch and still opens late packets of the previous one
/// (§7.2, §17.1). TestRatchet on the keys alone.
#[test]
fn ratchet_slots() {
    let (mut s, mut r) = keys(k0());
    s.ctr = (1 << EPOCH_BITS) - 3;
    let mut pkts = Vec::new();
    let mut entered = 0;
    for i in 0..6u8 {
        let (b, fresh) = seal(&mut s, TYPE_DATA, &[i]);
        entered += usize::from(fresh);
        assert_eq!(fresh, i == 3, "packet {i}");
        pkts.push(b);
    }
    assert!(entered == 1 && s.key.e == 1 && s.key.k == EpochKey::new(0, k0()).ratchet().k);
    for i in [0, 3, 1, 4, 2, 5] {
        let (pt, slot) = recv_keys(&mut r, &pkts[i]).unwrap_or_else(|| panic!("packet {i} not opened"));
        assert_eq!(pt, [i as u8]);
        assert_eq!(slot, [Slot::Cur, Slot::Prev, Slot::Prev, Slot::Next, Slot::Cur, Slot::Cur][i], "packet {i}");
        if i == 3 {
            assert_eq!(r.cur.e, 1, "receiver did not advance on the first packet of epoch 1");
        }
    }
    assert!(r.prev.e == 0 && r.prev.is_set() && r.next.e == 2 && r.next.k == r.cur.ratchet().k);
    // Ratcheting reads its key and changes nothing.
    let k = EpochKey::new(7, k0());
    let (a, b) = (k.ratchet(), k.ratchet());
    assert!(k.e == 7 && k.k == k0() && a.e == 8 && a.k == b.k && a.is_set());
    // The key message alone moves a receiver that missed a whole epoch.
    let (_, mut r2) = keys(k0());
    assert!(!r2.install_from(5, &s.key.k, 0));
    assert!(r2.cur.e == 5 && !r2.prev.is_set() && r2.next.e == 6);
}

/// Every case of install (§7.2, notes/core.md 2 and 31).
#[test]
fn install_cases() {
    let (_, mut r) = keys(k0());
    let k1 = EpochKey::new(0, k0()).ratchet();
    assert!(r.cur.e == 0 && r.cur.k == k0() && r.next.e == 1 && r.next.k == k1.k && !r.prev.is_set());
    assert!(r.win.check(9));
    let held = state(&r);
    // The same key again (a reconnect to the same process, A12): nothing changes.
    assert!(!r.install_from(0, &k0(), 0) && state(&r) == held);
    // A key that is not 32 bytes: nothing changes.
    assert!(
        !r.install_from(0, &[1; 31], 0)
            && !r.install_from(3, &[1; 33], 0)
            && !r.install_from(0, &[], 0)
            && state(&r) == held
    );
    // The next epoch's key: the receiver advances, as on its first packet.
    assert!(!r.install_from(1, &k1.k, 0));
    assert!(r.cur.e == 1 && r.prev.e == 0 && r.prev.k == k0() && r.next.e == 2 && r.win == held.win);
    // A key message that crossed packets of its epoch: the previous or the current key, ignored.
    let held = state(&r);
    assert!(!r.install_from(0, &k0(), 0) && !r.install_from(1, &k1.k, 0) && state(&r) == held);
    // A jump past the next epoch: the previous key goes, the window stays.
    let k9 = [9; 32];
    assert!(!r.install_from(4, &k9, 0));
    assert!(r.cur.e == 4 && r.cur.k == k9 && !r.prev.is_set() && r.next.e == 5 && r.win == held.win);
    // A key from outside the chain: the peer's session is new while mine lives on (note 31).
    assert!(r.install_from(4, &[8; 32], 0));
    assert!(r.cur.k == [8; 32] && !r.prev.is_set() && r.win == crate::wire::Window::default());
    assert!(r.install_from(2, &[7; 32], 0) && r.cur.e == 2, "an earlier epoch with another key is a new peer too");
    // Any epoch, even the last, without overflow: a jump, and the next epoch wraps to 0.
    assert!(!r.install_from(u64::MAX, &[6; 32], 0) && r.cur.e == u64::MAX && r.next.e == 0);
    let mut dst = [0; MAX_PACKET];
    assert_eq!(r.open(&mut dst, &forge_keys(0, 1)), None);
    let k = EpochKey::new(u64::MAX, [6; 32]).ratchet();
    assert!(
        !r.install_from(0, &k.k, 0) && r.cur.e == 0 && r.prev.e == u64::MAX,
        "the wrapped next epoch's key advances"
    );
}

/// A packet with counter `c` and rid 0x1234 that no key sealed.
fn forge_keys(c: u64, rid: u16) -> Vec<u8> {
    let mut b = vec![0; HEADER_LEN + DATA_LEN + 20 + TAG_LEN];
    b[0] = TYPE_DATA;
    b[1..3].copy_from_slice(&rid.to_be_bytes());
    b[3..HEADER_LEN].copy_from_slice(&c.to_be_bytes());
    b
}

/// A forged counter in any epoch, the next one included, opens nothing and costs no HMAC: the next
/// epoch's key was derived when the current one arrived, and open only reads (§7.2, §17.1).
/// TestForgedCounter on the keys alone.
#[test]
fn forged_counter_keys() {
    let (_, r) = keys(k0());
    assert!(r.next.is_set() && r.next.e == 1, "next epoch not derived on install");
    let mut dst = [0; MAX_PACKET];
    for c in [u64::MAX, 1 << EPOCH_BITS, (1 << EPOCH_BITS) + 1, 2 << EPOCH_BITS, 5] {
        let b = forge_keys(c, 0x1234);
        let before = allocs();
        assert_eq!(r.open(&mut dst, &b), None, "forged counter {c:#x}");
        assert_eq!(allocs(), before);
    }
}

/// The peer's next session, between the same two processes, sends a new key whose counters start
/// again: its packets open; the old session's do not (§7.2). TestPeerNewKey on the keys alone.
#[test]
fn peer_new_key_keys() {
    let (mut old, mut r) = keys(k0());
    let (p, _) = seal(&mut old, TYPE_DATA, b"old");
    assert!(recv_keys(&mut r, &p).is_some());
    let mut new = Sender { key: EpochKey::new(0, [0x77; 32]), ctr: 0, rid: 0x1234 };
    assert!(r.install_from(0, &new.key.k, 0), "a key from outside the chain is fresh");
    let (q, _) = seal(&mut new, TYPE_DATA, b"new");
    assert_eq!(recv_keys(&mut r, &q), Some((b"new".to_vec(), Slot::Cur)), "counter 0 again, under the new key");
    let (p2, _) = seal(&mut old, TYPE_DATA, b"old");
    assert!(recv_keys(&mut r, &p).is_none() && recv_keys(&mut r, &p2).is_none());
}

/// R2-09: a session that installs a key its peer already used (the peer's session lived on, mine
/// is new on a reused rid) drops every packet sealed before the key message, given the counter the
/// message carried; a key message without one (Go's: `c` 0) does not.
#[test]
fn counter_floor() {
    let mut old = Sender { key: EpochKey::new(0, k0()), ctr: 0, rid: 0x1234 };
    let recorded: Vec<_> = (0..1000u32).map(|i| seal(&mut old, TYPE_DATA, &i.to_be_bytes()).0).collect();
    let c = old.ctr;
    // Without the floor, the old session's packets replay into the new one.
    let mut r = Receiver::default();
    assert!(!r.install_from(0, &old.key.k, 0));
    assert!(recorded.iter().all(|p| recv_keys(&mut r, p).is_some()));
    // With it, none does, and what the peer seals after the message opens.
    let mut r = Receiver::default();
    assert!(!r.install_from(0, &old.key.k, c) && r.floor == c);
    assert!(recorded.iter().all(|p| recv_keys(&mut r, p).is_none()));
    let (p, _) = seal(&mut old, TYPE_DATA, b"new");
    assert_eq!(recv_keys(&mut r, &p), Some((b"new".to_vec(), Slot::Cur)));
    // The floor holds across the peer's next epochs, the key message's or a catch-up's.
    let k1 = old.key.ratchet();
    assert!(!r.install_from(1, &k1.k, 0) && r.floor == c && r.cur.e == 1);
    assert!(!r.install_from(9, &[3; 32], 0) && r.floor == c && r.cur.e == 9);
    // A key from outside the chain (the peer's new session) takes that message's counter.
    assert!(r.install_from(0, &[4; 32], 2) && r.floor == 2);
    let mut new = Sender { key: EpochKey::new(0, [4; 32]), ctr: 0, rid: 0x1234 };
    let pkts: Vec<_> = (0..3).map(|i: u8| seal(&mut new, TYPE_DATA, &[i]).0).collect();
    assert!(recv_keys(&mut r, &pkts[1]).is_none() && recv_keys(&mut r, &pkts[2]).is_some());
}

/// Challenges are deterministic per key, differ per lane, generation, seq and key, are never 0,
/// and allocate nothing (R2-03).
#[test]
fn challenger_tokens() {
    let (a, b) = (Challenger::new(k0()), Challenger::new([1; 32]));
    let mut seen = BTreeSet::new();
    let (mut made, mut tokens) = (0, [0; 3]);
    for lane in [0, 0x0101, 0x0201] {
        for agen in 0..3 {
            for seq in 0..100 {
                let before = allocs();
                tokens = [a.token(lane, agen, seq), a.token(lane, agen, seq), b.token(lane, agen, seq)];
                made += allocs() - before;
                let [t, again, other] = tokens;
                assert!(t != 0 && t == again && t != other);
                assert!(seen.insert(t), "token repeated at lane {lane:#x} gen {agen} seq {seq}");
            }
        }
    }
    assert_eq!(made, 0, "allocations making {} tokens", 3 * seen.len());
    assert_ne!(tokens[0], 0);
    assert_ne!(a.token(1, 0, 0), a.token(0, 1, 0));
    assert_ne!(a.token(0, 1, 0), a.token(0, 0, 1));
}

/// Receive ids: 0 is never given, held ids are skipped, a full table gives 0, and only the owner
/// frees an id (§7.2).
#[test]
fn rid_table() {
    let mut t = RidTable::new();
    assert_eq!((t.alloc(7), t.alloc(8), t.alloc(7)), (1, 2, 3));
    assert_eq!((t.lookup(1), t.lookup(2), t.lookup(4)), (Some(7), Some(8), None));
    t.free(2, 7); // not its owner's
    assert_eq!(t.lookup(2), Some(8));
    t.free(2, 8);
    assert_eq!(t.lookup(2), None);
    // The walk goes on past freed ids, wraps past 0 and skips held ones.
    let mut ids = BTreeSet::from([1, 3]);
    for _ in 0..65533 {
        let id = t.alloc(9);
        assert!(id != 0 && ids.insert(id), "id {id} given twice or 0");
    }
    assert_eq!((ids.len(), t.alloc(10)), (65535, 0), "a full table gives 0");
    t.free(40000, 9);
    assert_eq!((t.alloc(11), t.alloc(12)), (40000, 0));
}

/// Seal and open allocate nothing, across an epoch boundary too (§4.2, the zero-allocation steady
/// state).
#[test]
fn no_allocations() {
    let (mut s, mut r) = keys(k0());
    s.ctr = (1 << EPOCH_BITS) - 500;
    let (mut b, mut dst) = (vec![0; MAX_PACKET], vec![0; MAX_PACKET]);
    let before = allocs();
    for i in 0..1000 {
        let (n, _) = s.seal(&mut b, TYPE_DATA, 1400);
        let (m, slot) = r.open(&mut dst, &b[..n]).expect("opens");
        assert!(m == 1400 && r.win.check(s.ctr - 1));
        if slot == Slot::Next {
            r.advance();
        }
        assert_eq!(slot == Slot::Next, i == 500);
    }
    assert_eq!(allocs() - before, 0, "allocations sealing and opening 1,000 packets");
}

/// Arbitrary bytes open nothing, but for the genuine packet; nothing panics and `dst` is untouched.
/// FuzzOpen on the keys alone: random bytes, mutations of a genuine packet, and forged headers in
/// every accepted epoch.
#[test]
fn fuzz_open_keys() {
    let (mut s, r) = keys(k0());
    s.ctr = (1 << EPOCH_BITS) + 77; // so the previous, current and next epochs are all held
    let mut r = r;
    assert!(!r.install_from(1, &EpochKey::new(0, k0()).ratchet().k, 0) && r.prev.is_set());
    let (pkt, _) = seal(&mut s, TYPE_DATA, b"genuine");
    let mut rnd = DetRand(99);
    let mut dst = vec![0x33; MAX_PACKET];
    for i in 0..20_000 {
        let mut b = match i % 3 {
            0 => {
                let mut b = vec![0; (rnd.next() % (MAX_PACKET as u64 + 20)) as usize];
                rnd.fill(&mut b);
                b
            }
            1 => {
                let mut b = pkt.clone();
                let j = (rnd.next() % b.len() as u64) as usize;
                b[j] ^= 1 << (rnd.next() % 8);
                b
            }
            _ => {
                let mut b = vec![0; HEADER_LEN + TAG_LEN + (rnd.next() % 100) as usize];
                rnd.fill(&mut b);
                let c = ((rnd.next() % 3) << EPOCH_BITS) | (rnd.next() & 0xff_ffff);
                b[3..HEADER_LEN].copy_from_slice(&c.to_be_bytes());
                b
            }
        };
        if rnd.next().is_multiple_of(16) {
            b.truncate((rnd.next() % (b.len() as u64 + 1)) as usize);
        }
        assert_eq!(r.open(&mut dst, &b), None, "input {i} opened");
    }
    assert!(dst.iter().all(|&x| x == 0x33));
    assert_eq!(r.open(&mut dst, &pkt).map(|(n, _)| dst[..n].to_vec()), Some(b"genuine".to_vec()));
}

// ---------------------------------------------------------------------------------------------
// The Session-level ports.

/// Any non-authentic bytes are dropped with no reply and no state change; the genuine packet is
/// delivered once (§17.1). Go: TestSealOpen.
#[test]
fn seal_open() {
    let (_, mut s, pkt) = keyed();
    // keyed() seals 5 bytes of audio as one inner IPv4/UDP packet (28 bytes of headers).
    assert_eq!(pkt.len(), OVERHEAD + 33, "packet is {} bytes, want {}", pkt.len(), OVERHEAD + 33);
    for i in 0..pkt.len() {
        for bit in [1, 0x80] {
            let mut b = pkt.clone();
            b[i] ^= bit;
            let before = rx_state(&s);
            let out = recv(&mut s, &b);
            let changed = rx_state(&s) != before;
            assert!(
                out.is_empty() && !changed && s.last_auth.is_none(),
                "byte {i} flipped: {} actions, state changed {changed}",
                out.len()
            );
        }
    }
    let longer = [&pkt[..], &[0]].concat();
    for b in [&[][..], &pkt[..HEADER_LEN + TAG_LEN - 1], &pkt[..pkt.len() - 1], &longer] {
        assert!(recv(&mut s, b).is_empty(), "{}-byte packet got actions", b.len());
    }
    assert_eq!(delivered(&recv(&mut s, &pkt)), vec![(crate::FlowId(1), b"audio".to_vec())], "genuine packet");
    assert!(recv(&mut s, &pkt).is_empty(), "replay delivered");
}

/// TestRatchet crosses an epoch boundary: the sender ratchets and sends key, the receiver follows
/// on the first packet of the next epoch and still opens late packets of the previous one.
#[test]
fn ratchet() {
    let (mut c, mut s, _) = keyed();
    c.tx.ctr = (1 << EPOCH_BITS) - 3;
    let (mut pkts, mut keys) = (Vec::new(), 0);
    for _ in 0..6 {
        for a in handle(&mut c, 0, Event::Send { packet: &crate::pair::inner(FlowId(1), &[1]) }) {
            match a {
                Act::Send { data, .. } => pkts.push(data),
                Act::Control(m) => {
                    keys += 1;
                    assert!(m.t == "key" && m.e == 1 && m.k == c.tx.key.k, "epoch key message {m:?}");
                }
                Act::Deliver { .. } => {}
            }
        }
    }
    assert!(keys == 1 && c.tx.key.e == 1, "{keys} key messages, sender epoch {}", c.tx.key.e);
    for i in [0, 3, 1, 4, 2, 5] {
        assert_eq!(delivered(&recv(&mut s, &pkts[i])).len(), 1, "packet {i} not delivered");
        if i == 3 {
            assert_eq!(s.rx.cur.e, 1, "receiver did not advance on the first packet of epoch 1");
        }
    }
    let rx = &s.rx;
    assert!(rx.prev.e == 0 && rx.prev.is_set() && rx.next.e == 2 && rx.next.k == rx.cur.ratchet().k, "slots");
    // The key message alone also moves a receiver that missed the whole epoch.
    let (_, mut s2, _) = keyed();
    s2.rx.install_from(5, &c.tx.key.k, 0);
    assert!(s2.rx.cur.e == 5 && !s2.rx.prev.is_set(), "install of a later epoch");
}

/// TestPeerNewKey: the peer's session ended while mine lived on (its SESSION_LINGER ran out), and
/// its next session, between the same two processes, sends a new key whose counters start again.
/// They open and deliver; the old session's packets do not (§7.2).
#[test]
fn peer_new_key() {
    let (_, mut s, old) = keyed();
    assert_eq!(delivered(&recv(&mut s, &old)).len(), 1, "first session's packet not delivered");
    let mut c = Session::new(Role::Client, 13, DetRand::rand(13));
    let hello = Msg { t: "hello".into(), ..Msg::default() };
    let ck = key_msg(&handle(&mut c, 0, Event::Control(&hello)));
    let sk = key_msg(&handle(&mut s, 0, Event::Control(&Msg { mode: MODE_PERFORMANCE.into(), ..hello })));
    handle(&mut s, 0, Event::Control(&ck));
    handle(&mut c, 0, Event::Control(&sk));
    handle(&mut c, 0, Event::Links { links: &[], mesh: Some(MeshLane { sock: 1, local: C_MESH, peer: S_MESH }) });
    let out = handle(&mut c, 0, Event::Send { packet: &crate::pair::inner(crate::FlowId(1), b"audio") });
    let pkt = out
        .into_iter()
        .filter_map(|a| match a {
            Act::Send { data, .. } if data[0] & !SMALL_BIT == TYPE_DATA => Some(data),
            _ => None,
        })
        .next_back()
        .expect("no data packet sent");
    assert_eq!(delivered(&recv(&mut s, &pkt)), vec![(crate::FlowId(1), b"audio".to_vec())], "new key's packet");
    assert!(delivered(&recv(&mut s, &old)).is_empty(), "the old session's packet was delivered under the new key");
}

/// R2-09: my session ended while the peer's lived on (SESSION_LINGER), and my next session got the
/// old one's rid again and takes the peer's unchanged key. The peer's packets to the old session,
/// replayed, change nothing in the new one; what the peer seals after its key message opens. A key
/// message without a counter (a Go peer's) leaves the replays open, which is what the floor stops.
#[test]
fn new_session_on_an_old_rid() {
    let (mut c, _, first) = keyed();
    let flow = crate::FlowId(1);
    let sent = |c: &mut Session, d: &[u8]| -> Vec<Vec<u8>> {
        let out = handle(c, 0, Event::Send { packet: &crate::pair::inner(flow, d) });
        out.into_iter().filter_map(|a| if let Act::Send { data, .. } = a { Some(data) } else { None }).collect()
    };
    let mut old = vec![first];
    for i in 0..20u8 {
        old.extend(sent(&mut c, &[i]));
    }
    let hello = Msg { t: "hello".into(), ..Msg::default() };
    let next = |c: &mut Session, floor: bool| {
        let mut s = Session::new(Role::Anchor, 14, DetRand::rand(14));
        let sk = key_msg(&handle(&mut s, 0, Event::Control(&Msg { mode: MODE_PERFORMANCE.into(), ..hello.clone() })));
        let mut ck = key_msg(&handle(c, 0, Event::Control(&hello)));
        assert!(sk.rid == c.tx.rid && ck.k == c.tx.key.k && ck.c == c.tx.ctr && ck.c > 0, "{sk:?} {ck:?}");
        if !floor {
            ck.c = 0;
        }
        handle(c, 0, Event::Control(&sk));
        handle(&mut s, 0, Event::Control(&ck));
        handle(&mut s, 0, Event::Links { links: &[], mesh: Some(MeshLane { sock: 1, local: S_MESH, peer: C_MESH }) });
        s
    };
    let mut s = next(&mut c, false);
    assert!(old.iter().any(|p| !delivered(&recv(&mut s, p)).is_empty()), "without a floor the replays open");
    let mut s = next(&mut c, true);
    let held = rx_state(&s);
    for p in &old {
        assert!(recv(&mut s, p).is_empty(), "an old session's packet did something");
    }
    assert_eq!(rx_state(&s), held, "an old session's packet moved the receiver");
    let new = sent(&mut c, b"new");
    let got: Vec<_> = new.iter().flat_map(|p| delivered(&recv(&mut s, p))).collect();
    assert!(got.contains(&(flow, b"new".to_vec())), "the packet sealed after the key message: {got:?}");
}

/// The next epoch's key was derived once, when the current one arrived; forged counters in any
/// epoch, the next one included, cost no HMAC and change nothing. Go: TestForgedCounter.
#[test]
fn forged_counter() {
    let (_, mut s, _) = keyed();
    assert!(s.rx.next.is_set() && s.rx.next.e == 1, "next epoch not derived on install");
    let before = rx_state(&s);
    for c in [u64::MAX, 1 << EPOCH_BITS, (1 << EPOCH_BITS) + 1, 2 << EPOCH_BITS, 5] {
        let b = forge(&s, c);
        assert!(recv(&mut s, &b).is_empty() && rx_state(&s) == before, "forged counter {c:#x} changed state");
    }
}

/// A random payload for FuzzPeer: a probe with random echoes, a data payload with a random port,
/// flow and seq, or random bytes, of random length.
fn peer_payload(r: &mut DetRand) -> (u8, Vec<u8>, u16) {
    let gap = (r.next() % 2000) as u16;
    match r.next() % 4 {
        0 => {
            let n = (r.next() % 4) as usize;
            let lanes = [0, 0x0101, 0x0201, r.next() as u16];
            let echoes: Vec<_> = (0..n)
                .map(|_| Echo {
                    lane: lanes[(r.next() % 4) as usize],
                    flags: r.next() as u8,
                    seq: r.next() as u32 % 50,
                    held: r.next() as u32,
                    owd: r.next() as i64,
                    last: r.next() % 100,
                    token: r.next(),
                    ..Echo::default()
                })
                .collect();
            let p = Probe {
                lane: lanes[(r.next() % 4) as usize],
                seq: r.next() as u32,
                sent_at: r.next() as i64,
                flags: r.next() as u8,
                n,
            };
            let mut b = vec![0; MAX_PACKET];
            let len = p.encode(&echoes, &mut b);
            b.truncate(len);
            (TYPE_PROBE, b, gap)
        }
        1 => {
            let mut b = vec![0; DATA_LEN + (r.next() % 1400) as usize];
            r.fill(&mut b);
            b[..2].copy_from_slice(&[0xbb, 0x7e]);
            let typ = if r.next().is_multiple_of(2) { TYPE_DATA } else { TYPE_DATA | SMALL_BIT };
            (typ, b, gap)
        }
        _ => {
            let mut b = vec![0; (r.next() % (MAX_PACKET - HEADER_LEN - TAG_LEN) as u64) as usize];
            r.fill(&mut b);
            (r.next() as u8, b, gap)
        }
    }
}

/// FuzzPeer seals arbitrary payloads with the peer's real key, as a buggy or hostile peer could:
/// probes with any echoes and lanes, data for any flow and seq. Nothing may panic or exceed the
/// session's bounds. Go's seeds and its committed crash input, then a seeded loop.
#[test]
fn fuzz_peer() {
    let mut b = vec![0; MAX_PACKET];
    let echoes = [
        Echo { lane: 0, seq: 1, held: 5, last: 3, ..Echo::default() },
        Echo { lane: 0x0101, seq: 1 << 31, ..Echo::default() },
    ];
    let n = Probe { lane: 0, seq: 9, sent_at: 1, flags: 1, n: 2 }.encode(&echoes, &mut b);
    let mut cases = vec![
        (TYPE_PROBE, b[..n].to_vec(), 0),
        (TYPE_DATA, vec![0xbb, 0x7e, 0, 1, 0, 0, 0, 7, b'v'], 30),
        (TYPE_DATA | SMALL_BIT, vec![0xbb, 0x80, 0, 2, 0xff, 0xff, 0xff, 0xff], 1000),
        (TYPE_DATA, b"00000000".to_vec(), 1000), // core/testdata/fuzz/FuzzPeer/29cd1c3e671b2d70
    ];
    let mut r = DetRand(0xf022);
    cases.extend((0..300).map(|_| peer_payload(&mut r)));
    for (typ, payload, gap) in cases {
        let (mut c, mut s, _) = keyed();
        let mut now = 0;
        for i in 0..3u8 {
            // The payload, then two variants, spaced by gap ms.
            let mut buf = vec![0; MAX_PACKET];
            let n = payload.len();
            buf[HEADER_LEN..HEADER_LEN + n].copy_from_slice(&payload);
            if i > 0 && n > 7 {
                buf[HEADER_LEN + 7] = buf[HEADER_LEN + 7].wrapping_add(i); // another seq or probe field
            }
            let (len, _) = c.tx.seal(&mut buf, typ, n);
            now += Mono::from(gap) * crate::MS;
            handle(&mut s, now, Event::Packet { sock: 1, from: C_MESH, data: &buf[..len] });
            handle(&mut s, now + SEC, Event::Tick);
            let st = s.snapshot();
            assert!(
                st.bytes <= BOND_BYTES && st.flows.len() <= MAX_FLOWS,
                "bounds: {} bytes, {} flows",
                st.bytes,
                st.flows.len()
            );
        }
    }
}

/// FuzzOpen feeds arbitrary bytes to a keyed session: nothing but the genuine packet may produce
/// an action, and nothing may panic. Go's seeds, then a seeded loop of random bytes, mutations of
/// the genuine packet and forged headers.
#[test]
fn fuzz_open() {
    let (_, s, pkt) = keyed();
    let mut inputs = vec![pkt.clone(), forge(&s, u64::MAX)];
    let mut r = DetRand(0x0be7);
    for i in 0..300 {
        inputs.push(match i % 3 {
            0 => {
                let mut b = vec![0; (r.next() % (MAX_PACKET as u64 + 20)) as usize];
                r.fill(&mut b);
                b
            }
            1 => {
                let mut b = pkt.clone();
                let j = (r.next() % b.len() as u64) as usize;
                b[j] ^= 1 << (r.next() % 8);
                b
            }
            _ => forge(&s, r.next() % (3 << EPOCH_BITS)),
        });
    }
    for b in inputs {
        let (_, mut s, pkt) = keyed();
        let out = recv(&mut s, &b);
        assert!(out.is_empty() || b == pkt, "non-genuine bytes produced {} actions", out.len());
    }
}
