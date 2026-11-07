//! Sealing, the key ratchet, the receiver's epoch slots, path-validation challenges and receive
//! ids (§7.2, §7.3, §7.5; Go: core/keys.go).
//!
//! Owner: C1.
//!
//! Superseded and dropped keys are zeroed (§7.2 forward secrecy per epoch; notes/open.md
//! "superseded epoch keys are not zeroed"). [`EpochKey`] zeroes its key bytes on drop, and the AES
//! round keys and GHASH key inside its AEAD zero themselves (aes-gcm's `zeroize` feature); every
//! slot change (ratchet, advance, install) drops the key it replaces. The HMAC that derives a key
//! zeroes its state and output (the `zeroize` features of hmac and sha2): the inner state of a
//! superseded key would derive every later key. Safe Rust cannot reach the copies the compiler
//! leaves when it moves a value, nor the key bytes of a key message once node has sent it.

use aes_gcm::aead::inout::InOutBuf;
use aes_gcm::aead::{AeadInOut, Nonce};
use aes_gcm::{Aes256Gcm, KeyInit};
use hmac::digest::CtOutput;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

use crate::wire::{Window, be};
use crate::{EPOCH_BITS, EPOCH_LABEL, HEADER_LEN, RidTable, TAG_LEN};

/// One epoch's AES-256-GCM key (Go: `epochKey`); `aead` None is an empty slot, as the default is
/// (Go: `epochKey{}`). Not Clone, so key material is never duplicated by accident; its bytes are
/// zeroed on drop.
#[derive(Default)]
pub(crate) struct EpochKey {
    pub(crate) e: u64,
    pub(crate) k: [u8; 32],
    pub(crate) aead: Option<Aes256Gcm>,
}

impl EpochKey {
    /// The key for epoch `e` (a 32-byte key never fails). Go: `newKey`.
    pub(crate) fn new(e: u64, k: [u8; 32]) -> EpochKey {
        EpochKey { e, k, aead: Aes256Gcm::new_from_slice(&k).ok() }
    }

    /// Whether the slot holds a key.
    pub(crate) fn is_set(&self) -> bool {
        self.aead.is_some()
    }

    /// The next epoch's key, K_{e+1} = HMAC-SHA256(K_e, EPOCH_LABEL) (§7.2), written straight into
    /// the slot that zeroes it. The epoch wraps rather than overflow: a key message may carry any
    /// `e`.
    pub(crate) fn ratchet(&self) -> EpochKey {
        let mut next = EpochKey { e: self.e.wrapping_add(1), k: [0; 32], aead: None };
        next.k.copy_from_slice(hmac(&self.k, &[EPOCH_LABEL]).as_bytes());
        next.aead = Aes256Gcm::new_from_slice(&next.k).ok();
        next
    }
}

impl Drop for EpochKey {
    /// Zeroes the key bytes; `aead` then zeroes its round keys as it drops.
    fn drop(&mut self) {
        self.k.zeroize();
    }
}

/// HMAC-SHA256 under `key` over the concatenation of `msg`; the output zeroes itself on drop.
fn hmac(key: &[u8; 32], msg: &[&[u8]]) -> CtOutput<Hmac<Sha256>> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes a key of any length");
    msg.iter().for_each(|p| m.update(p));
    m.finalize()
}

/// The nonce of the packet whose header is `h`: 2 zero bytes ‖ rid ‖ counter (§7.3).
fn nonce(h: &[u8]) -> Nonce<Aes256Gcm> {
    let mut n = [0; 12];
    n[2..].copy_from_slice(&h[1..HEADER_LEN]);
    n.into()
}

/// My send key toward the peer. One counter covers every packet type and lane, so the nonce
/// 0 ‖ rid ‖ counter never repeats under a key (§7.3). Go: `sender` (its nonce scratch is a
/// local in Rust).
pub(crate) struct Sender {
    pub(crate) key: EpochKey,
    /// The next counter.
    pub(crate) ctr: u64,
    /// The peer's receive id, from its key message; 0 until then.
    pub(crate) rid: u16,
}

impl Sender {
    /// Seals `b[HEADER_LEN..HEADER_LEN + n]` in place under the next counter, writing the header
    /// before it and the tag after it, and returns the packet's length (HEADER_LEN + n + TAG_LEN)
    /// and whether the key entered a new epoch, which the peer must hear in a key message. `b`
    /// is a whole pool buffer. Allocates nothing; entering an epoch costs one HMAC.
    pub(crate) fn seal(&mut self, b: &mut [u8], typ: u8, n: usize) -> (usize, bool) {
        let c = self.ctr;
        self.ctr = c.wrapping_add(1);
        let fresh = c >> EPOCH_BITS != self.key.e;
        if fresh {
            self.key = self.key.ratchet(); // drops, so zeroes, the old key
        }
        b[0] = typ;
        b[1..3].copy_from_slice(&self.rid.to_be_bytes());
        b[3..HEADER_LEN].copy_from_slice(&c.to_be_bytes());
        let (h, rest) = b.split_at_mut(HEADER_LEN);
        let (body, tail) = rest.split_at_mut(n);
        let aead = self.key.aead.as_ref().expect("a send key is always set");
        let tag = aead.encrypt_inout_detached(&nonce(h), h, body.into()).expect("a packet is far below GCM's limits");
        tail[..TAG_LEN].copy_from_slice(&tag);
        (HEADER_LEN + n + TAG_LEN, fresh)
    }
}

/// Which receiver slot opened a packet. Go compared the returned `*epochKey` with `&s.rx.next`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Prev,
    Cur,
    Next,
}

/// The peer's keys for the previous, current and next epoch, and the session's replay window
/// (§7.2, §7.5); the default has no key. Go: `receiver`.
#[derive(Default)]
pub(crate) struct Receiver {
    pub(crate) prev: EpochKey,
    pub(crate) cur: EpochKey,
    pub(crate) next: EpochKey,
    pub(crate) win: Window,
    /// Counters below this are dropped before any work: the peer's counter when it sent the key
    /// this chain started from (R2-09, [`Receiver::install_from`]); 0 when it did not say.
    pub(crate) floor: u64,
}

impl Receiver {
    /// Authenticates lane packet `b` (§7.5 steps 1–2), writing its plaintext to the front of `dst`
    /// (a MAX_PACKET buffer), and returns the plaintext's length and the slot that opened it. A
    /// counter whose epoch is not the previous, current or next one is dropped before any work, so
    /// a forged counter costs no HMAC and changes nothing (§7.6 rule 1); so is one below the floor
    /// (R2-09). The caller has checked HEADER_LEN + TAG_LEN ≤ b.len() ≤ MAX_PACKET; any other
    /// length, or a `dst` too short, is dropped too. `dst` is untouched unless the packet opens.
    pub(crate) fn open(&self, dst: &mut [u8], b: &[u8]) -> Option<(usize, Slot)> {
        let n = b.len().checked_sub(HEADER_LEN + TAG_LEN)?;
        let (h, rest) = b.split_at(HEADER_LEN);
        let (k, slot) = self.key(be(&h[3..]))?;
        let (ct, tag) = rest.split_at(n);
        let buf = InOutBuf::new(ct, dst.get_mut(..n)?).ok()?;
        k.aead.as_ref()?.decrypt_inout_detached(&nonce(h), h, buf, tag.try_into().ok()?).ok()?;
        Some((n, slot))
    }

    /// §10.4 step 1 for counter `c`, without opening anything: the key of its epoch, if that is
    /// the previous, current or next one and `c` is not below the floor.
    pub(crate) fn key(&self, c: u64) -> Option<(&EpochKey, Slot)> {
        if !self.cur.is_set() || c < self.floor {
            return None;
        }
        // The epoch's distance from the current one, which wraps as Go's uint64 does.
        match (c >> EPOCH_BITS).wrapping_sub(self.cur.e) {
            0 => Some((&self.cur, Slot::Cur)),
            1 => Some((&self.next, Slot::Next)),
            u64::MAX if self.prev.is_set() => Some((&self.prev, Slot::Prev)),
            _ => None,
        }
    }

    /// Takes the peer's key for epoch `e` from a key message and derives the next epoch's with one
    /// HMAC. Reports whether the key is from outside the chain held, which means the peer's
    /// session is new while mine lives on (§7.6 rule 8): the window restarts and the caller
    /// forgets what the peer numbered (`Session::peer_new`). A key message that crossed its
    /// epoch's packets is ignored; a jump past the next epoch drops the previous key, which cannot
    /// be derived backwards. A key that is not 32 bytes changes nothing. Go: `install`.
    ///
    /// `c` is the peer's next counter when it sent the message (`Msg::c`, R2-09; 0 from a Go
    /// peer, which is Go's behavior). On the first key and on a key from outside the chain,
    /// counters below `c` are dropped from then on: they were sealed before the message, so they
    /// are replays of an earlier session's packets into a session that took the peer's unchanged
    /// key on a reused rid. Not on a catch-up past the next epoch, whose first packets are sealed
    /// before its key message.
    pub(crate) fn install_from(&mut self, e: u64, k: &[u8], c: u64) -> bool {
        // The message's key, zeroed whichever way this returns.
        let Ok(key) = <[u8; 32]>::try_from(k).map(Zeroizing::new) else { return false };
        let mut fresh = false;
        if !self.cur.is_set() {
            self.floor = c;
        } else if e == self.cur.e && *key == self.cur.k || e.wrapping_add(1) == self.cur.e && *key == self.prev.k {
            return false;
        } else if e == self.cur.e.wrapping_add(1) && *key == self.next.k {
            self.advance();
            return false;
        } else if e > self.cur.e.wrapping_add(1) {
            self.prev = EpochKey::default();
        } else {
            (self.prev, self.win, self.floor, fresh) = (EpochKey::default(), Window::default(), c, true);
        }
        self.cur = EpochKey::new(e, *key);
        self.next = self.cur.ratchet();
        fresh
    }

    /// Makes the next epoch current once one of its packets passed the session replay window,
    /// drops (zeroing) the oldest key and derives the new next one (§7.2). The slots swap in
    /// place, so no key passes through a temporary.
    pub(crate) fn advance(&mut self) {
        std::mem::swap(&mut self.prev, &mut self.cur);
        std::mem::swap(&mut self.cur, &mut self.next);
        self.next = self.cur.ratchet(); // drops the old previous key
    }
}

/// Path-validation challenges (R2-03, notes/open.md; §7.6 rule 2, §8.2). A probe's challenge is
/// a keyed PRF of (lane, address generation, seq), so the peer cannot predict the challenge of a
/// probe sent to an address it does not hold, nothing is stored per probe, and rand is read once
/// per session rather than on the send path.
pub(crate) struct Challenger {
    /// Drawn from the session's rand at start; never leaves the session; zeroed on drop.
    key: Zeroizing<[u8; 32]>,
}

impl Challenger {
    pub(crate) fn new(key: [u8; 32]) -> Challenger {
        Challenger { key: Zeroizing::new(key) }
    }

    /// The challenge of probe `seq` on lane `lane` sent to address generation `agen`: the first
    /// 8 bytes of HMAC-SHA256 under `key` over the three, big-endian (lane 2, agen 4, seq 4). Never
    /// 0, which means "no challenge" on the wire (0 becomes 1). Allocates nothing.
    pub(crate) fn token(&self, lane: u16, agen: u32, seq: u32) -> u64 {
        let t = hmac(&self.key, &[&lane.to_be_bytes(), &agen.to_be_bytes(), &seq.to_be_bytes()]);
        be(&t.as_bytes()[..8]).max(1)
    }
}

impl RidTable {
    /// Gives `owner` an unused receive id, walking the 16-bit space; 0 is never used and means the
    /// table is full.
    pub(crate) fn alloc(&mut self, owner: u64) -> u16 {
        // The 2^16 ids after the last one given, ending with that one.
        let mut ids = (1..=1 << 16).map(|k: u32| self.next.wrapping_add(k as u16));
        let Some(rid) = ids.find(|r| *r != 0 && !self.owners.contains_key(r)) else { return 0 };
        self.next = rid;
        self.owners.insert(rid, owner);
        rid
    }

    /// Frees `rid` if `owner` holds it.
    pub(crate) fn free(&mut self, rid: u16, owner: u64) {
        if self.owners.get(&rid) == Some(&owner) {
            self.owners.remove(&rid);
        }
    }
}

#[cfg(test)]
mod tests;
