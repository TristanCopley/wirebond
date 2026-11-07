//! The replay window and the probe codec (§7.5, §8.2; Go: core/wire.go).
//!
//! Owner: C1. Integers are big-endian on the wire.
//!
//! R2-03 (notes/open.md): path validation needs an unpredictable challenge. It travels in the
//! echo's 18-byte Observed slot (bytes 35–52 of an echo), which core never read, so ProbeLen,
//! EchoLen and MaxEchoes are unchanged:
//!
//! - bytes 35–42 `token`: the challenge of the newest probe received on the echoed lane, returned;
//! - bytes 43–50 `chal`: on the echo of the probe's own lane, that probe's own challenge; 0 in
//!   every other echo;
//! - bytes 51–52: written 0, ignored on read (a Go peer writes the observed port there).
//!
//! A Go peer still writes an address in the slot, so its echoes never validate a Rust peer's path
//! until Go takes the same change (notes/rust.md).

use crate::{ECHO_LEN, PROBE_LEN, REPLAY_WINDOW};

/// The words of the replay bitmap.
const WORDS: u64 = REPLAY_WINDOW / 64;

/// A replay window over REPLAY_WINDOW counters in the style of RFC 6479: a 1 KB bitmap that slides
/// by 64-bit words (§7.5 steps 3 and 4). Go: `window`, whose `any` flag is not needed here: the
/// default, top 0 with no bit set, takes counter 0 as new, and sliding it from there clears only
/// words that are 0 already.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    /// The highest counter recorded; 0 before any.
    pub(crate) top: u64,
    pub(crate) bits: [u64; WORDS as usize],
}

impl Default for Window {
    fn default() -> Window {
        Window { top: 0, bits: [0; WORDS as usize] }
    }
}

impl Window {
    /// Records counter `c` and reports whether it was new and inside the window. A counter above
    /// the top slides the window, clearing the words it passes; a counter WORDS or more words below
    /// the top's word is outside, so the window spans 8,129 to 8,192 counters (note 3).
    pub(crate) fn check(&mut self, c: u64) -> bool {
        if c > self.top {
            for i in self.top / 64 + 1..=(c / 64).min(self.top / 64 + WORDS) {
                self.bits[(i % WORDS) as usize] = 0;
            }
            self.top = c;
        } else if self.top / 64 - c / 64 >= WORDS {
            return false;
        }
        let (word, bit) = (&mut self.bits[(c / 64 % WORDS) as usize], 1u64 << (c % 64));
        let fresh = *word & bit == 0;
        *word |= bit;
        fresh
    }

    /// Whether `check(c)` would report counter `c` not new: recorded, or below the window. Reads
    /// only, so an unauthenticated counter can ask it (§10.4: a later copy is not opened).
    pub(crate) fn seen(&self, c: u64) -> bool {
        c <= self.top
            && (self.top / 64 - c / 64 >= WORDS || self.bits[(c / 64 % WORDS) as usize] & (1u64 << (c % 64)) != 0)
    }
}

/// The sealed payload of a TYPE_PROBE packet (§8.2), fixed-width. Go: `probe`. The echoes live
/// beside it in an `[Echo; MAX_ECHOES]` buffer (Go's `echoes []echo` pointed into one), so a
/// probe holds only their count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Probe {
    /// Client link id << 8 | server link id; 0 is the Tailscale lane.
    pub(crate) lane: u16,
    /// Per lane and sender; only increases.
    pub(crate) seq: u32,
    /// The sender's clock, µs.
    pub(crate) sent_at: i64,
    /// PROBE_COPY: this lane is in the sender's copy set; PROBE_ANSWER: answer at once.
    pub(crate) flags: u8,
    /// The echoes that follow, in the echo buffer the probe was decoded into or encoded from.
    pub(crate) n: usize,
}

/// What a probe's sender received on one lane from the peer (§8.2). Go: `echo`, with `observed`
/// replaced by `token` and `chal` (R2-03, module doc).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Echo {
    pub(crate) lane: u16,
    /// Bit 0: lagging (§8.4).
    pub(crate) flags: u8,
    /// The newest probe received on the lane.
    pub(crate) seq: u32,
    /// µs from that probe's arrival to this probe's sending.
    pub(crate) held: u32,
    /// µs, arrival − SentAt of that probe.
    pub(crate) owd: i64,
    /// Packets received on the lane at that probe's arrival (wraps).
    pub(crate) pkts: u32,
    /// Bytes, same snapshot (wraps).
    pub(crate) bytes: u32,
    /// Counter of the newest data packet received on the lane.
    pub(crate) last: u64,
    /// The challenge probe `seq` carried, returned; 0 if it carried none.
    pub(crate) token: u64,
    /// In the echo of the probe's own lane: that probe's challenge; 0 otherwise.
    pub(crate) chal: u64,
}

impl Probe {
    /// Writes the probe and `echoes` (its first `self.n`) into `b`, which must hold
    /// PROBE_LEN + ECHO_LEN × n bytes, and returns that length.
    pub(crate) fn encode(&self, echoes: &[Echo], b: &mut [u8]) -> usize {
        b[..2].copy_from_slice(&self.lane.to_be_bytes());
        b[2..6].copy_from_slice(&self.seq.to_be_bytes());
        b[6..14].copy_from_slice(&self.sent_at.to_be_bytes());
        (b[14], b[15]) = (self.flags, self.n as u8);
        for (e, o) in echoes[..self.n].iter().zip(b[PROBE_LEN..].as_chunks_mut::<ECHO_LEN>().0) {
            o[..2].copy_from_slice(&e.lane.to_be_bytes());
            o[2] = e.flags;
            o[3..7].copy_from_slice(&e.seq.to_be_bytes());
            o[7..11].copy_from_slice(&e.held.to_be_bytes());
            o[11..19].copy_from_slice(&e.owd.to_be_bytes());
            o[19..23].copy_from_slice(&e.pkts.to_be_bytes());
            o[23..27].copy_from_slice(&e.bytes.to_be_bytes());
            o[27..35].copy_from_slice(&e.last.to_be_bytes());
            o[35..43].copy_from_slice(&e.token.to_be_bytes());
            o[43..51].copy_from_slice(&e.chal.to_be_bytes());
            o[51..].fill(0);
        }
        probe_len(self.n)
    }

    /// Parses `b` into the probe, keeping its echoes in `buf`, and reports whether `b` is a whole
    /// probe with at most `buf.len()` echoes. Never panics on any input (fuzzed).
    pub(crate) fn decode(&mut self, b: &[u8], buf: &mut [Echo]) -> bool {
        let Some(n) = b.get(15).map(|&n| usize::from(n)) else { return false };
        if b.len() != probe_len(n) || n > buf.len() {
            return false;
        }
        *self = Probe {
            lane: be(&b[..2]) as u16,
            seq: be(&b[2..6]) as u32,
            sent_at: be(&b[6..14]) as i64,
            flags: b[14],
            n,
        };
        for (e, o) in buf.iter_mut().zip(b[PROBE_LEN..].as_chunks::<ECHO_LEN>().0) {
            *e = Echo {
                lane: be(&o[..2]) as u16,
                flags: o[2],
                seq: be(&o[3..7]) as u32,
                held: be(&o[7..11]) as u32,
                owd: be(&o[11..19]) as i64,
                pkts: be(&o[19..23]) as u32,
                bytes: be(&o[23..27]) as u32,
                last: be(&o[27..35]),
                token: be(&o[35..43]),
                chal: be(&o[43..51]),
            };
        }
        true
    }
}

/// The big-endian number in `b`, at most 8 bytes: a copy and one byte swap, where a byte-by-byte
/// fold cost a few instructions per byte on every packet.
pub(crate) fn be(b: &[u8]) -> u64 {
    let mut a = [0; 8];
    a[8 - b.len()..].copy_from_slice(b);
    u64::from_be_bytes(a)
}

/// The length of a probe with `n` echoes.
pub(crate) const fn probe_len(n: usize) -> usize {
    PROBE_LEN + n * ECHO_LEN
}

/// Whether probe or flow Seq `a` comes after `b` in serial order (RFC 1982), so comparisons survive
/// the wrap (R2-24; Go compared with `>`).
pub(crate) fn after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Whether probe Seq `seq` is newer than `top`, the newest one seen; `top` 0 means none yet, after
/// which any Seq is newer. Seq 0 is never a probe.
pub(crate) fn seq_newer(seq: u32, top: u32) -> bool {
    seq != 0 && (top == 0 || after(seq, top))
}

#[cfg(test)]
mod tests;
