//! Packet buffers: the session's pool of MAX_PACKET-byte buffers, the packet record that pacer
//! queues, rewind rings and reorder buffers hold, and the FIFO ring (Go: core/buf.go).
//!
//! Owner: C1. Actions reach [`crate::Out`] synchronously inside `Session::handle`, so a buffer can
//! be reused the moment it is put back: Go's `later` list and `flush` (which kept a buffer alive
//! until the next Handle because its Action pointed into it) are gone.

use std::collections::VecDeque;

use crate::{BOND_BYTES, MAX_PACKET, Mono};

/// A buffer's index in the [`Pool`]. Go: a pool index, `int32`, with -1 for none; Rust uses
/// `Option<BufIx>` for "none".
pub(crate) type BufIx = u32;

/// Hands out MAX_PACKET-byte buffers without allocating once warm (Go: `pool`). A buffer has one
/// owner at a time (a pacer queue, a rewind ring or a reorder buffer) or is transient.
///
/// The buffers live in one slab: buffer `i` is `slab[i * MAX_PACKET..(i + 1) * MAX_PACKET]`. The
/// slab grows (and moves) only in [`Pool::get`], while warming up, so never hold a slice of it
/// across a `get`. The default pool has no buffer yet.
#[derive(Default)]
pub(crate) struct Pool {
    /// Every buffer, back to back.
    slab: Vec<u8>,
    /// Indices of free buffers; the next `get` takes the last.
    free: Vec<BufIx>,
}

impl Pool {
    /// A free buffer, or None when the session's buffers are at BOND_BYTES (§7.6 rule 5). The
    /// pool doubles when empty (at least 16 buffers), so it grows only while warming up and keeps
    /// slack afterwards.
    pub(crate) fn get(&mut self) -> Option<BufIx> {
        if self.free.is_empty() && self.len() < MAX_BUFS {
            let have = self.len();
            let n = have.max(16).min(MAX_BUFS - have);
            // Exactly: the last step would otherwise reserve twice the 16 MiB cap.
            self.slab.reserve_exact(n * MAX_PACKET);
            self.slab.resize((have + n) * MAX_PACKET, 0);
            // Room for every buffer, so `put` never allocates.
            self.free.reserve_exact(have + n);
            self.free.extend((have..have + n).map(|i| i as BufIx));
        }
        self.free.pop()
    }

    /// Frees buffer `i` at once (Go deferred it to the next Handle; see the module doc). It must be
    /// a buffer `get` gave and not yet put back.
    pub(crate) fn put(&mut self, i: BufIx) {
        self.free.push(i);
    }

    /// The bytes of buffers in use.
    pub(crate) fn held(&self) -> usize {
        (self.len() - self.free.len()) * MAX_PACKET
    }

    /// The number of buffers allocated, free or not.
    pub(crate) fn len(&self) -> usize {
        self.slab.len() / MAX_PACKET
    }

    /// Buffer `i`, all MAX_PACKET bytes of it.
    pub(crate) fn buf(&self, i: BufIx) -> &[u8] {
        &self.slab[i as usize * MAX_PACKET..][..MAX_PACKET]
    }

    /// Buffer `i`, all MAX_PACKET bytes of it, to fill or seal in place.
    pub(crate) fn buf_mut(&mut self, i: BufIx) -> &mut [u8] {
        &mut self.slab[i as usize * MAX_PACKET..][..MAX_PACKET]
    }

    /// Copies the first `n` bytes of buffer `from` into buffer `to` (bulk's extra lanes, §11).
    pub(crate) fn copy(&mut self, from: BufIx, to: BufIx, n: usize) {
        let (f, t) = (from as usize * MAX_PACKET, to as usize * MAX_PACKET);
        self.slab.copy_within(f..f + n, t);
    }
}

/// The most buffers a session may hold (§7.6 rule 5).
pub(crate) const MAX_BUFS: usize = BOND_BYTES / MAX_PACKET;

/// One sealed bulk packet in a pacer queue, a rewind ring or a reorder buffer, or a small packet's
/// counter in a rewind ring (`buf` None). Go: `pkt`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pkt {
    /// Counter; in a reorder buffer, the flow seq.
    pub(crate) c: u64,
    /// When it entered this queue, was sent, or arrived.
    pub(crate) at: Mono,
    /// Its pool buffer; None for a small packet's counter in a rewind ring, or an empty reorder
    /// slot.
    pub(crate) buf: Option<BufIx>,
    /// Length in bytes.
    pub(crate) n: u32,
}

/// A FIFO of packets that doubles when full and never shrinks, so a warm one allocates nothing
/// (Go: `ring`). `push` is `push_back`, `pop` is `pop_front` (Option, where Go's popped an empty
/// ring's garbage), `at(i)` is `[i]`, `n` is `len()`.
pub(crate) type Ring = VecDeque<Pkt>;

#[cfg(test)]
mod tests;
